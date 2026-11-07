# Tool permissions

By default mu runs every tool call without asking. `[permissions]` in `~/.mu/config.toml`, `--permissions` and `/permissions` decide when a call needs your approval instead.

```toml
[permissions]
mode = "yolo"          # ask | edits | yolo (default: yolo)
readonly_bash = true   # the built-in read-only command set counts as reading

[permissions.rules]
bash = { "cargo test *" = "allow", "git push *" = "deny", "rm *" = "ask" }
edit = { "*.lock" = "ask" }
"mcp__github_*" = "allow"   # a string decides every call of the tool
```

`--permissions ask|edits|yolo` overrides the mode for one run, and `/permissions` shows or sets it while mu runs (it works mid-turn). The status line shows the mode whenever it is not `yolo`. Plan mode is independent: it is still read-only whatever the permission mode.

## Modes

| Mode | Runs without asking | Asks |
|---|---|---|
| `ask` | reads: `read`, `grep`, `glob`, `todo`, `ask`, `web_search`, `fetch`, `memory`, … and read-only `bash` | everything that writes or runs |
| `edits` | the same, plus file edits inside the project | commands, and writes outside the project or into `.git` |
| `yolo` | everything | only what a rule says to ask |

`local://` files (plans, notes) count as reading, as do the `artifact://`, `skill://` and `mu://` pages the tools refuse to write. Anything under the project's `.git` or under `~/.mu` is never covered by `edits`. Subagents run with the same mode and the same session grants.

`readonly_bash = false` makes every `bash` call ask unless a rule allows it. The read-only set covers programs such as `ls`, `cat`, `grep`, `git status`, `git log`, `git diff`, `find` without `-delete`/`-exec`, and pipelines and loops made only of those; anything the classifier is not sure about is not read-only.

## Rules

Keys are tool-name globs, values are `allow`, `ask` or `deny`, either for every call of the tool (`"mcp__github_*" = "allow"`) or for commands and paths that match a pattern:

- `bash`: every simple command of the line, as written (`cargo test --release`), including those inside `$(…)`, loops and subshells.
- `write`, `edit`, `read`, `grep`, `glob`: each target path, relative to the project when inside it.
- `fetch`: the URL.
- Other tools: only `"*"` and whole-tool string values match.

`*` matches anything, including `/` and spaces; `?` matches one character; everything else is literal. A pattern ending in ` *` also matches the text before it, so `cargo test *` matches `cargo test`.

A `deny` always wins, then an `ask`, and only then does a covering `allow` (or the read-only classifier) let the call run. `yolo` still honours `deny` and `ask` rules. A command that does not parse, or one that starts with an assignment (`FOO=1 cmd`), can never be covered by an `allow` rule.

## The prompt

When a call needs approval, mu shows what it would do and why, with these answers:

1. **Yes** — run it this once.
2. **Yes, don't ask again this session for …** — remember the grant until mu exits.
3. **Yes, always in this project for …** — save it under `~/.mu/permissions/`, per checkout (worktrees share it). Never in the repository.
4. **Yes, always everywhere for …** — add the rule to `~/.mu/config.toml`.
5. **No** — do not run it. Esc is No. Tab (or Right) attaches a note for the model first.

The grant offered is the program plus its subcommand (`cargo test *`), the file paths a write or edit touches, or the site of a `fetch`. Options 2–4 appear only when a grant can be expressed.

A denial with a note lets the turn continue: the model sees `Not run: the user denied this bash call: <note>` and can try something else. A denial without a note ends the turn, as Esc does.

Rules that deny show up in the tool result: `Not run: permission rule "git push *" = "deny" for bash (~/.mu/config.toml).` Rules are read at startup; a run already going does not see an edit made meanwhile.

## Subagents and headless runs

A `task` subagent inherits the mode and the session grants, and its prompts are forwarded to your UI labelled with the subagent that asked (`from Task1 (task)`).

A `-p` run has nobody to ask, so a call that needs approval does not run: stderr says `approval needed: bash: touch x` and the model is told nobody can approve it in this run. Use `--permissions yolo` or a rule when a scripted run must be allowed to act. `mu deliver` runs its subprocesses with `--permissions yolo`: approving the plan is the authorization, and deny rules still apply.
